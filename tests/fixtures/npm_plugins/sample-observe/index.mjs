// 集成测试用的最小插件：注册一个 notify 模式的 turn.end Handler。
//
// notify 是只读通知类 Hook，handler 不返回结果即可（runner 对 null 回 `continue`）。

export function activate(context) {
  context.hooks.on({
    id: "on-turn-end",
    hook: "turn.end",
    // 必须与 manifest 声明的 mode 一致，否则握手会因「运行期 Handler 与 manifest 不一致」失败。
    mode: "notify",
    // notify 是只读通知：不返回结果（runner 对 null 默认回 `continue`）。
    handler: () => undefined,
  });
}

export function deactivate() {}
