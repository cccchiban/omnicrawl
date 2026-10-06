// 集成测试用的最小插件：注册一个 observe 模式的 turn.end Handler。
//
// 返回值固定为 `continue` 并带上注解，供 worker_lifecycle 断言「真实调用一次 Handler」。

export function activate(context) {
  context.hooks.on({
    id: "on-turn-end",
    hook: "turn.end",
    // 必须与 manifest 声明的 mode 一致，否则握手会因「运行期 Handler 与 manifest 不一致」失败。
    mode: "notify",
    handler: (event) => ({
      action: "continue",
      annotations: [`fixture observed ${event?.eventId ?? "unknown"}`],
    }),
  });
}

export function deactivate() {}
