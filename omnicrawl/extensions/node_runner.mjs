#!/usr/bin/env node
/**
 * OmniCrawl 单插件 Node runner。
 *
 * Host 固定调用：
 *   node node_runner.mjs --plugin-root <canonical-path>
 *
 * 协议：stdin/stdout NDJSON JSON-RPC 2.0。
 * 插件日志经 SDK logger 走 stderr 结构化行；普通 console 也被重定向到 stderr。
 */

import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import readline from "node:readline";
import fs from "node:fs";

const require = createRequire(import.meta.url);

function parseArgs(argv) {
  const args = { pluginRoot: "" };
  for (let i = 0; i < argv.length; i += 1) {
    const item = argv[i];
    if (item === "--plugin-root") {
      args.pluginRoot = argv[i + 1] || "";
      i += 1;
    }
  }
  return args;
}

function writeStdout(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function writeStderrLine(level, message, fields = {}) {
  const payload = {
    level,
    message: String(message),
    fields,
    ts: new Date().toISOString(),
  };
  process.stderr.write(`${JSON.stringify(payload)}\n`);
}

// 强制把 console 导向 stderr，避免污染协议 stdout。
const originalConsole = { ...console };
for (const level of ["log", "info", "warn", "error", "debug"]) {
  console[level] = (...parts) => {
    writeStderrLine(level === "log" ? "info" : level, parts.map(String).join(" "));
  };
}

function createLogger() {
  return {
    debug(message, fields) {
      writeStderrLine("debug", message, fields || {});
    },
    info(message, fields) {
      writeStderrLine("info", message, fields || {});
    },
    warn(message, fields) {
      writeStderrLine("warn", message, fields || {});
    },
    error(message, fields) {
      writeStderrLine("error", message, fields || {});
    },
  };
}

function definePlugin(plugin) {
  return plugin;
}

function loadPackageJson(pluginRoot) {
  const packagePath = path.join(pluginRoot, "package.json");
  const raw = fs.readFileSync(packagePath, "utf8");
  return JSON.parse(raw);
}

async function importPlugin(entryPath) {
  const url = pathToFileURL(entryPath).href;
  const mod = await import(url);
  return mod.default || mod.plugin || mod;
}

class PluginRuntime {
  constructor(pluginRoot) {
    this.pluginRoot = path.resolve(pluginRoot);
    this.packageJson = loadPackageJson(this.pluginRoot);
    this.manifest = this.packageJson.omnicrawl || {};
    this.handlers = new Map();
    this.plugin = null;
    this.initialized = false;
    this.activeCancellations = new Set();
    this.logger = createLogger();
    this._hostRequestId = 1;
    this._pendingHostRequests = new Map();
  }

  /** Host 对 Worker 请求的响应入口（custom.emit 等）。 */
  resolveHostResponse(message) {
    if (!message || message.id == null) return false;
    const pending = this._pendingHostRequests.get(message.id);
    if (!pending) return false;
    this._pendingHostRequests.delete(message.id);
    if (message.error) {
      pending.reject(new Error(message.error.message || String(message.error)));
    } else {
      pending.resolve(message.result || { ok: true });
    }
    return true;
  }

  _requestHost(method, params = {}) {
    const id = this._hostRequestId;
    this._hostRequestId += 1;
    return new Promise((resolve, reject) => {
      this._pendingHostRequests.set(id, { resolve, reject });
      writeStdout({
        jsonrpc: "2.0",
        id,
        method,
        params,
      });
      // Host 超时由 Python 侧控制；Worker 侧给一个安全兜底。
      setTimeout(() => {
        if (this._pendingHostRequests.has(id)) {
          this._pendingHostRequests.delete(id);
          reject(new Error(`Host 请求超时：${method}`));
        }
      }, 10000);
    });
  }

  async initialize(params = {}) {
    if (this.initialized) {
      return this._registrationSnapshot();
    }
    const entryRel = this.manifest.entry || this.packageJson.main;
    if (!entryRel) {
      throw new Error("package.json 缺少 omnicrawl.entry / main");
    }
    const entryPath = path.resolve(this.pluginRoot, entryRel);
    const rootResolved = path.resolve(this.pluginRoot);
    if (!entryPath.startsWith(rootResolved + path.sep) && entryPath !== rootResolved) {
      throw new Error(`entry 逃逸包根目录：${entryRel}`);
    }

    const imported = await importPlugin(entryPath);
    const definition = typeof imported === "function" ? imported({ definePlugin }) : imported;
    if (!definition || typeof definition.activate !== "function") {
      throw new Error("插件必须导出 activate(context) 的 PluginDefinition");
    }
    this.plugin = definition;

    const declared = Array.isArray(this.manifest.hooks) ? this.manifest.hooks : [];
    const declaredIds = new Set(declared.map((item) => item.id));

    const context = {
      plugin: {
        name: this.packageJson.name,
        version: this.packageJson.version,
        apiVersion: String(this.manifest.apiVersion || "1"),
      },
      runtime: {
        omnicrawlVersion: params.omnicrawlVersion || process.env.OMNICRAWL_VERSION || "0.1.4",
        nodeVersion: process.version,
      },
      hooks: {
        on: (registration) => {
          if (!registration || typeof registration !== "object") {
            throw new Error("hooks.on(registration) 需要对象");
          }
          const id = String(registration.id || "");
          const hook = String(registration.hook || "");
          const mode = String(registration.mode || "observe");
          const handler = registration.handler;
          if (!declaredIds.has(id)) {
            throw new Error(`运行期不能注册 manifest 未声明的 Handler：${id}`);
          }
          if (typeof handler !== "function") {
            throw new Error(`Handler ${id} 缺少 handler 函数`);
          }
          this.handlers.set(id, { id, hook, mode, handler });
          return () => {
            this.handlers.delete(id);
          };
        },
        emitCustom: async (event, payload = {}, options = {}) => {
          if (!this.initialized) {
            throw new Error("initialize 完成前不能 emitCustom");
          }
          const permissions = Array.isArray(this.manifest.permissions)
            ? this.manifest.permissions
            : [];
          if (!permissions.includes("hook:custom-emit")) {
            throw new Error("缺少 hook:custom-emit 权限");
          }
          const version =
            options.version != null
              ? Number(options.version)
              : Number(
                  (Array.isArray(this.manifest.customEvents)
                    ? this.manifest.customEvents.find((item) => item.name === event)
                    : null)?.version || 1
                );

          // 同插件订阅必须本地投递：Host 若在 hook.invoke 中再回调本 Worker
          // 会与“等待 custom.emit 响应”形成死锁。
          let localDelivered = 0;
          const localEvent = {
            apiVersion: String(this.manifest.apiVersion || "1"),
            hook: event,
            payload: payload || {},
            timestamp: new Date().toISOString(),
          };
          for (const binding of this.handlers.values()) {
            if (binding.hook !== event) continue;
            await binding.handler(localEvent);
            localDelivered += 1;
          }

          const result = await this._requestHost("custom.emit", {
            event,
            version,
            payload: payload || {},
            localDelivered,
          });
          return {
            ...(result || { ok: true }),
            localDelivered,
            delivered: Number(result?.delivered || 0) + localDelivered,
          };
        },
      },
      logger: this.logger,
    };

    await definition.activate(context);

    // 若插件未在 activate 中显式 on()，但 manifest 声明了 hooks，则保持空绑定；
    // Host 会在二次校验时发现实际注册子集。
    this.initialized = true;
    return this._registrationSnapshot();
  }

  _registrationSnapshot() {
    const registrations = [];
    for (const item of this.handlers.values()) {
      registrations.push({
        id: item.id,
        hook: item.hook,
        mode: item.mode,
      });
    }
    // 若 activate 没手动 on，回落为 manifest 声明（开发期便利）；
    // Host 仍要求它们是 manifest 子集。
    if (registrations.length === 0 && Array.isArray(this.manifest.hooks)) {
      for (const item of this.manifest.hooks) {
        registrations.push({
          id: item.id,
          hook: item.hook,
          mode: item.mode,
        });
      }
    }
    return {
      plugin: {
        name: this.packageJson.name,
        version: this.packageJson.version,
        apiVersion: String(this.manifest.apiVersion || "1"),
      },
      handlers: registrations,
      capabilities: {
        customEmit: Boolean(
          Array.isArray(this.manifest.permissions) &&
            this.manifest.permissions.includes("hook:custom-emit")
        ),
      },
    };
  }

  async invoke(handlerId, event) {
    const binding = this.handlers.get(handlerId);
    if (!binding) {
      // 允许仅声明未绑定：默认 continue，便于 observe 空实现。
      return { action: "continue" };
    }
    const result = await binding.handler(event);
    if (result == null) {
      return { action: "continue" };
    }
    if (typeof result !== "object") {
      throw new Error(`Handler ${handlerId} 必须返回对象`);
    }
    return result;
  }

  async shutdown() {
    if (this.plugin && typeof this.plugin.deactivate === "function") {
      await this.plugin.deactivate();
    }
    this.handlers.clear();
    this.initialized = false;
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (!args.pluginRoot) {
    writeStderrLine("error", "missing --plugin-root");
    process.exit(2);
  }
  const runtime = new PluginRuntime(args.pluginRoot);
  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

  const sendResult = (id, result) => {
    writeStdout({ jsonrpc: "2.0", id, result });
  };
  const sendError = (id, code, message) => {
    writeStdout({ jsonrpc: "2.0", id, error: { code, message } });
  };

  // 必须用事件驱动读取 stdin：handler 内 await custom.emit 时 Host 响应
  // 也要从同一 stdin 进来。若 for-await 串行阻塞，会形成死锁超时。
  let shuttingDown = false;
  const inflight = new Set();

  const handleMessage = async (message) => {
    // Host 对 Worker 请求的响应：无 method，带 id+result/error。
    if (
      message.id != null &&
      (Object.prototype.hasOwnProperty.call(message, "result") ||
        Object.prototype.hasOwnProperty.call(message, "error")) &&
      !message.method
    ) {
      runtime.resolveHostResponse(message);
      return;
    }

    const { id, method, params } = message;
    try {
      if (method === "initialize") {
        const result = await runtime.initialize(params || {});
        if (id !== undefined) sendResult(id, result);
      } else if (method === "hook.invoke") {
        const handlerId = params?.handlerId;
        const event = params?.event || {};
        const result = await runtime.invoke(handlerId, event);
        if (id !== undefined) sendResult(id, result);
      } else if (method === "hook.cancel") {
        const requestId = params?.requestId;
        if (requestId != null) runtime.activeCancellations.add(requestId);
        if (id !== undefined) sendResult(id, { ok: true });
      } else if (method === "ping") {
        if (id !== undefined) sendResult(id, { ok: true, ts: Date.now() });
      } else if (method === "shutdown") {
        shuttingDown = true;
        await runtime.shutdown();
        if (id !== undefined) sendResult(id, { ok: true });
        process.exit(0);
      } else if (method) {
        if (id !== undefined) sendError(id, -32601, `Method not found: ${method}`);
      }
    } catch (error) {
      const text = error instanceof Error ? error.message : String(error);
      writeStderrLine("error", text);
      if (id !== undefined) sendError(id, -32000, text);
    }
  };

  rl.on("line", (line) => {
    if (shuttingDown) return;
    const trimmed = String(line || "").trim();
    if (!trimmed) return;
    let message;
    try {
      message = JSON.parse(trimmed);
    } catch (error) {
      writeStderrLine("error", "invalid json from host", { error: String(error) });
      return;
    }
    if (!message || typeof message !== "object") return;
    const task = handleMessage(message);
    inflight.add(task);
    task.finally(() => inflight.delete(task));
  });

  await new Promise((resolve) => {
    rl.on("close", resolve);
  });
  await Promise.allSettled([...inflight]);
}

main().catch((error) => {
  writeStderrLine("error", error instanceof Error ? error.message : String(error));
  process.exit(1);
});

export { definePlugin };
