// 集成测试用的最小插件：注册一个 observe 模式的 turn.end Handler。
//
// 返回值固定为 `continue` 并带上注解，供 worker_lifecycle 断言「真实调用一次 Handler」。

export function activate(context) {
  context.hooks.on({
    id: "on-turn-end",
    hook: "turn.end",
    mode: "observe",
    handler: (event) => ({
      action: "continue",
      annotations: [`fixture observed ${event?.eventId ?? "unknown"}`],
    }),
  });
}

export function deactivate() {}
